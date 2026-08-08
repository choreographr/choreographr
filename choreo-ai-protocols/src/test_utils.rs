//! Shared test helpers for integration tests (feature `test-utils`).
//!
//! This module is compiled only when the `test-utils` feature is enabled,
//! which happens exclusively for test builds: this crate's own integration
//! tests enable it via the self dev-dependency, and `choreo-daemon`'s
//! integration tests enable it on their dev-dependency. It therefore never
//! leaks into the published public API.
//!
//! The [`MockProvider`] here is the single scripted HTTP provider used by the
//! reasoning round-trip wire tests in both `choreo-ai-protocols/tests/` and
//! `choreo-daemon/tests/` — a `TcpListener` serving one canned response per
//! request and recording every request head/body so tests can assert on the
//! actual wire.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One HTTP request captured by the mock provider.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    /// Header lines captured verbatim (lowercased name → value).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl CapturedRequest {
    /// Look up a request header by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == &name.to_ascii_lowercase())
            .map(|(_, v)| v.as_str())
    }

    /// Parse the captured request body as JSON.
    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("captured request body is JSON")
    }
}

/// A tiny scripted HTTP provider: serves one canned response per request, in
/// order (the last entry repeats for any excess requests), and records every
/// request so tests can assert on the wire.
///
/// Only speaks the subset of HTTP/1.1 the ureq agents need: reads the request
/// head + `Content-Length` body, replies with `Connection: close` (so the
/// client opens a fresh connection per request), and drops the stream.
pub struct MockProvider {
    addr: std::net::SocketAddr,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    _handle: std::thread::JoinHandle<()>,
}

impl MockProvider {
    /// `responses`: `(status, content_type, body)` served in order; the last
    /// entry repeats for any excess requests.
    pub fn start(responses: Vec<(u16, &'static str, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        let addr = listener.local_addr().expect("mock provider local addr");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_thread = Arc::clone(&captured);
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));

        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let mut responses = responses.lock().unwrap();

                // Read the request head (through the blank line) and then the
                // Content-Length body, recording both for later assertions.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        // Client hung up mid-request; move on.
                        break 0;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos;
                    }
                    if buf.len() > 1 << 20 {
                        panic!("mock provider: request head too large");
                    }
                };
                if head_end == 0 {
                    continue;
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let body_start = head_end + 4;
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        if !key.trim().eq_ignore_ascii_case("content-length") {
                            return None;
                        }
                        value.trim().parse::<usize>().ok()
                    })
                    .unwrap_or(0);
                while buf.len().saturating_sub(body_start) < content_length {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = buf[body_start..body_start + content_length].to_vec();

                let mut parts = head.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                // Capture headers (everything after the request line, before the
                // blank line) so tests can assert on outbound header values.
                let headers = head
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                captured_thread.lock().unwrap().push(CapturedRequest {
                    method,
                    path,
                    headers,
                    body,
                });

                // Serve the next scripted response (peek when it is the last
                // one so excess requests still get an answer).
                let (status, content_type, response_body) = if responses.len() > 1 {
                    responses.pop_front().expect("scripted response")
                } else {
                    responses.front().cloned().expect("scripted response")
                };
                let reason = if status == 200 { "OK" } else { "Error" };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                stream.write_all(head.as_bytes()).unwrap_or_default();
                stream
                    .write_all(response_body.as_bytes())
                    .unwrap_or_default();
                stream.flush().unwrap_or_default();
            }
        });

        Self {
            addr,
            captured,
            _handle: handle,
        }
    }

    /// Base URL for the given path prefix (e.g. `"v1"` → `http://…/v1`).
    pub fn base_url(&self, prefix: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}",
            self.addr.port(),
            prefix.trim_matches('/')
        )
    }

    /// Every request captured so far, in arrival order.
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.captured.lock().unwrap().clone()
    }
}
