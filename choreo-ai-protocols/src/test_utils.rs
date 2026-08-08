//! Shared test helpers for integration tests (feature `test-utils`).
//!
//! This module is compiled only when the `test-utils` feature is enabled,
//! which happens exclusively for test builds: this crate's own integration
//! tests enable it via the self dev-dependency, and `choreo-daemon`'s
//! integration tests enable it on their dev-dependency. It therefore never
//! leaks into the published public API.
//!
//! Because it is consumed cross-crate (daemon integration tests link it as a
//! dev-dependency), it cannot be gated with `#[cfg(test)]` — that flag is only
//! set for the crate under test, never for its dependencies. The `test-utils`
//! feature gate is the sanctioned stand-in: it is the one exception to
//! AGENTS.md's rule that `unwrap`/`expect`/`panic!` live only in `#[cfg(test)]`
//! modules and `tests/` files, and it is never compiled into a production
//! build.
//!
//! The [`MockProvider`] here is the single scripted HTTP provider used by the
//! reasoning round-trip wire tests in both `choreo-ai-protocols/tests/` and
//! `choreo-daemon/tests/` — a `TcpListener` serving one canned response per
//! request and recording every request head/body so tests can assert on the
//! actual wire.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
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
///
/// The serve thread runs a non-blocking accept + poll loop so that dropping
/// the provider (which sets the `shutdown` flag and joins the thread) can
/// terminate it promptly instead of leaking a permanently blocked `accept`
/// thread per test.
pub struct MockProvider {
    addr: std::net::SocketAddr,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    /// Set by `Drop` to ask the serve thread to exit its poll loop. This is a
    /// test-only cooperative cancellation flag (single-bit, no data), the one
    /// shared-state pattern AGENTS.md sanctions; a channel message could not
    /// interrupt a thread blocked in `accept`.
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockProvider {
    /// `responses`: `(status, content_type, body)` served in order; the last
    /// entry repeats for any excess requests.
    pub fn start(responses: Vec<(u16, &'static str, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        // Non-blocking so the serve loop can poll `accept` and observe the
        // shutdown flag instead of blocking forever on a connection that never
        // arrives.
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("mock provider local addr");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_thread = Arc::clone(&captured);
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_thread = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            // Handle one accepted connection: read the request head + body,
            // record it, and serve the next scripted response. Kept as a
            // closure so the poll loop below stays readable.
            let serve_request = |mut stream: std::net::TcpStream| {
                // A short read timeout keeps the blocking request reads
                // interruptible: if Drop sets the shutdown flag while a client
                // is half-open (e.g. a test panicked mid-request), the read
                // returns a timeout instead of blocking the join forever.
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(100)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];

                // Abort the read loops when the provider is being dropped, so
                // a half-open connection cannot stall `MockProvider::drop`'s
                // thread join.
                let shutdown_requested = || shutdown_thread.load(Ordering::SeqCst);

                let head_end = loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break 0, // client hung up mid-request; move on
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break pos;
                            }
                            if buf.len() > 1 << 20 {
                                panic!("mock provider: request head too large");
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) =>
                        {
                            if shutdown_requested() {
                                return;
                            }
                            continue;
                        }
                        Err(_) => return, // connection error; move on
                    }
                };
                if head_end == 0 {
                    return;
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
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) =>
                        {
                            if shutdown_requested() {
                                return;
                            }
                            continue;
                        }
                        Err(_) => return,
                    }
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
                // Recover from a poisoned lock (e.g. a test thread that
                // panicked while holding it) instead of aborting this serve
                // thread, which would stall `Drop`'s join and leak the thread.
                captured_thread
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(CapturedRequest {
                        method,
                        path,
                        headers,
                        body,
                    });

                // Serve the next scripted response (peek when it is the last
                // one so excess requests still get an answer). The lock is
                // taken only for this pop/peek — holding it across the
                // blocking reads above would let an in-flight request stall
                // `requests()` from the test thread.
                let (status, content_type, response_body) = {
                    let mut responses = responses.lock().unwrap_or_else(|e| e.into_inner());
                    if responses.len() > 1 {
                        responses.pop_front().expect("scripted response")
                    } else {
                        responses.front().cloned().expect("scripted response")
                    }
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
            };

            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // The accepted socket inherits the listener's
                        // non-blocking mode on some platforms; restore blocking
                        // so the request reader works.
                        stream.set_nonblocking(false).expect("stream blocking");
                        serve_request(stream);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // No pending connection: poll the shutdown flag and
                        // sleep briefly before trying again, so Drop can
                        // terminate this thread promptly.
                        if shutdown_thread.load(Ordering::SeqCst) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break, // listener closed
                }
            }
        });

        Self {
            addr,
            captured,
            shutdown,
            handle: Some(handle),
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
        self.captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        // Signal the serve thread to stop polling and join it, so tests do
        // not leak a blocked accept thread per MockProvider.
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
