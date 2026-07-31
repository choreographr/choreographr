use std::io::{Read, Write};
use std::net::TcpListener;

use choreographr::tools::http::{HttpRequestArgs, execute_http_request_tool};

/// Spawn a minimal HTTP server for testing the http_request tool.
fn spawn_http_tool_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind http tool server");
    let addr = listener.local_addr().expect("http tool server addr");
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                break;
            };
            std::thread::spawn(move || {
                let mut buf = vec![0_u8; 32 * 1024];
                let mut total_read = 0usize;
                let mut content_length = None;

                loop {
                    if total_read >= buf.len() {
                        break;
                    }
                    let Ok(n) = stream.read(&mut buf[total_read..]) else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    total_read += n;

                    if content_length.is_none() {
                        let data = &buf[..total_read];
                        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers_end = pos + 4;
                            let headers_str =
                                std::str::from_utf8(&data[..headers_end]).unwrap_or("");
                            for line in headers_str.lines() {
                                if line.to_ascii_lowercase().starts_with("content-length:")
                                    && let Some(len_str) = line.split_once(':')
                                {
                                    let len_str = len_str.1.trim();
                                    if let Ok(len) = len_str.parse::<usize>() {
                                        content_length = Some(len);
                                    }
                                }
                            }
                            let body_received = data.len().saturating_sub(headers_end);
                            if content_length.is_none() || body_received >= content_length.unwrap()
                            {
                                break;
                            }
                        }
                    } else if let Some(expected) = content_length {
                        let data = &buf[..total_read];
                        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers_end = pos + 4;
                            let body_received = data.len().saturating_sub(headers_end);
                            if body_received >= expected {
                                break;
                            }
                        }
                    }
                }

                let request = String::from_utf8_lossy(&buf[..total_read]).to_string();
                let first_line = request.lines().next().unwrap_or_default().to_string();

                if first_line.starts_with("HEAD /meta ") {
                    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 42\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                if first_line.starts_with("GET /range ") {
                    let range = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, value)| value.trim().to_string())
                        .unwrap_or_default();
                    let body = "abcdefghij";
                    let response = format!(
                        "HTTP/1.1 206 Partial Content\r\ncontent-type: text/plain\r\ncontent-length: {}\r\ncontent-range: bytes 0-9/100\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    assert_eq!(range, "bytes=0-9");
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                if first_line.starts_with("GET /binary ") {
                    let body = [0_u8, 159, 146, 150];
                    let header = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                    return;
                }

                if first_line.starts_with("GET /long ") {
                    let body = "x".repeat(200 * 1024);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                if first_line.starts_with("POST /echo ") {
                    let body = request
                        .split_once("\r\n\r\n")
                        .map(|(_, body)| body)
                        .unwrap_or_default();
                    let response_body = format!("echo:{body}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                let body = "not found";
                let response = format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    (format!("http://{addr}"), handle)
}

#[ignore]
#[test]
fn http_request_tool_supports_range_header() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "GET".into(),
            url: format!("{base_url}/range"),
            headers: [("Range".into(), "bytes=0-9".into())].into(),
            body: None,
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(
        content.contains("status: 206 Partial Content"),
        "{}",
        content
    );
    assert!(
        content.contains("content-range: bytes 0-9/100"),
        "{}",
        content
    );
    assert!(content.ends_with("abcdefghij"), "{}", content);

    drop(server);
}

#[ignore]
#[test]
fn http_request_tool_supports_head_requests() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "HEAD".into(),
            url: format!("{base_url}/meta"),
            headers: Default::default(),
            body: None,
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("status: 200 OK"), "{}", content);
    assert!(content.contains("accept-ranges: bytes"), "{}", content);
    assert!(content.ends_with("\n\n"), "{}", content);

    drop(server);
}

#[ignore]
#[test]
fn http_request_tool_summarizes_non_text_responses() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "GET".into(),
            url: format!("{base_url}/binary"),
            headers: Default::default(),
            body: None,
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(
        content.contains("content-type: application/octet-stream"),
        "{}",
        content
    );
    assert!(
        content.ends_with("body omitted: non-text response"),
        "{}",
        content
    );

    drop(server);
}

#[ignore]
#[test]
fn http_request_tool_truncates_large_text_responses() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "GET".into(),
            url: format!("{base_url}/long"),
            headers: Default::default(),
            body: None,
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("...[truncated]"), "{}", content);

    drop(server);
}

#[ignore]
#[test]
fn http_request_tool_supports_post_body() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "POST".into(),
            url: format!("{base_url}/echo"),
            headers: Default::default(),
            body: Some("hello".into()),
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.ends_with("echo:hello"), "{}", content);

    drop(server);
}
