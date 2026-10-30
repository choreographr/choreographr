//! Shared test helpers for integration tests (feature `test-utils`).
//!
//! This module is compiled only when the `test-utils` feature is enabled,
//! which happens exclusively for test builds: this crate's own integration
//! tests enable it via the self dev-dependency, and `choreographr`'s
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
//! actual wire. A provider whose reply depends on what it was sent (a backend
//! that rejects a specific request shape, say) uses
//! [`MockProvider::start_responding`] instead of a script, so one mock covers
//! both the permissive and the refusing case.

// Test-only mock-provider infrastructure, compiled solely under the
// `test-utils` feature (see the module docs above). Clippy's test allowances
// (clippy.toml) only recognize `#[cfg(test)]`/`#[test]` contexts, not feature
// gates, so the sanctioned unwrap/expect/panic/indexing exception is granted
// here at module level — mirroring the AGENTS.md test-only exception.
#![expect(clippy::expect_used, clippy::indexing_slicing)]

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One HTTP request captured by the mock provider.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    /// HTTP method (e.g. `POST`).
    pub method: String,
    /// Request path (e.g. `/v1/messages`).
    pub path: String,
    /// Header lines captured verbatim (lowercased name → value).
    pub headers: Vec<(String, String)>,
    /// Raw request body bytes.
    pub body: Vec<u8>,
}

impl CapturedRequest {
    /// Look up a request header by name (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == &name.to_ascii_lowercase())
            .map(|(_, v)| v.as_str())
    }

    /// Parse the captured request body as JSON.
    ///
    /// # Panics
    ///
    /// Panics if the captured body is not valid JSON.
    #[must_use]
    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("captured request body is JSON")
    }
}

/// One HTTP response: `(status, content type, body)`.
pub type MockResponse = (u16, &'static str, String);

/// A per-request responder: given the request just recorded, the response to
/// send. Lets a mock model a backend that answers by *what it was asked* (a
/// 400 for a request shape it refuses, a different turn per conversation
/// state) instead of by a fixed order.
pub type MockResponder = dyn Fn(&CapturedRequest) -> MockResponse + Send + Sync;

/// What the serve thread replies with.
enum Script {
    /// Serve these responses in order; the last one repeats for any excess
    /// request.
    Sequence(Arc<Mutex<VecDeque<MockResponse>>>),
    /// Decide every response from its own request.
    PerRequest(Arc<MockResponder>),
}

/// A tiny scripted HTTP provider: serves one canned response per request, in
/// order (the last entry repeats for any excess requests), and records every
/// request so tests can assert on the wire.
///
/// Only speaks the subset of HTTP/1.1 the ureq agents need: reads the request
/// head + `Content-Length` body, replies with `Connection: close` (so the
/// client opens a fresh connection per request), and drops the stream.
///
/// The serve thread blocks in `accept`; dropping the provider sets the
/// `shutdown` flag and connects once to wake it, so teardown costs one connect
/// and never a poll interval or a sleep.
pub struct MockProvider {
    addr: std::net::SocketAddr,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    /// Set by `Drop` to ask the serve thread to exit. This is a test-only
    /// cooperative cancellation flag (single-bit, no data), the one shared-state
    /// pattern AGENTS.md sanctions; a channel message could not interrupt a
    /// thread blocked in `accept`.
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockProvider {
    /// `responses`: `(status, content_type, body)` served in order; the last
    /// entry repeats for any excess requests.
    ///
    /// # Panics
    ///
    /// Panics if the TCP listener cannot be bound or configured; the mock
    /// provider has no non-panicking fallback.
    #[must_use]
    pub fn start(responses: Vec<MockResponse>) -> Self {
        // `start_scripted` with no address to embed — the common shape.
        Self::start_scripted(move |_| responses)
    }

    /// Like [`MockProvider::start`], but the script is built AFTER the bind
    /// and receives the bound base root (`http://<addr>`) so a response body
    /// can embed a URL pointing back at this very server (the z.ai adapter
    /// tests' `data[0].url` CDN download — the port does not exist before
    /// the bind, so the body cannot carry it).
    /// # Panics
    ///
    /// Panics if the TCP listener cannot be bound or configured; the mock
    /// provider has no non-panicking fallback.
    pub fn start_scripted<F>(make: F) -> Self
    where
        F: FnOnce(&str) -> Vec<MockResponse>,
    {
        // Bind first so the scripted responses can embed the real address,
        // THEN build them — the serve thread is spawned below, so the script
        // is complete before any traffic arrives.
        let (listener, addr) = bind_mock_listener();
        let responses = Arc::new(Mutex::new(VecDeque::from(make(&format!("http://{addr}")))));
        Self::start_with(listener, addr, Script::Sequence(responses))
    }

    /// Serve whatever `respond` decides for each request, instead of a fixed
    /// script.
    ///
    /// This is how a mock enforces a backend's *rejections*: a request the real
    /// backend would refuse gets its 4xx here, so a test can never go green on a
    /// body the provider would have answered with an error. Nothing is consumed
    /// either, so a rejected request cannot silently eat the response a later
    /// (legal) request needs — which is why the decision belongs in the
    /// responder rather than in the order requests happen to arrive.
    ///
    /// # Panics
    ///
    /// Panics if the TCP listener cannot be bound or configured; the mock
    /// provider has no non-panicking fallback.
    pub fn start_responding<F>(respond: F) -> Self
    where
        F: Fn(&CapturedRequest) -> MockResponse + Send + Sync + 'static,
    {
        let (listener, addr) = bind_mock_listener();
        Self::start_with(listener, addr, Script::PerRequest(Arc::new(respond)))
    }

    /// Spawn the serve thread for an already-bound listener.
    fn start_with(listener: TcpListener, addr: std::net::SocketAddr, script: Script) -> Self {
        let script = Arc::new(script);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_thread = Arc::clone(&captured);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_thread = Arc::clone(&shutdown);
        let script_thread = Arc::clone(&script);

        let handle = std::thread::spawn(move || {
            // Handle one accepted connection: read the request head + body,
            // record it, and reply. Kept as a closure so the accept loop below
            // stays readable.
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
                            assert!(
                                buf.len() <= 1 << 20,
                                "mock provider: request head too large"
                            );
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
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(CapturedRequest {
                        method,
                        path,
                        headers,
                        body,
                    });

                // Ask the script for this response. The sequence lock is taken
                // only for the pop/peek — holding it across the blocking reads
                // above would let an in-flight request stall `requests()` from
                // the test thread.
                let (status, content_type, response_body) = match &*script_thread {
                    Script::Sequence(responses) => {
                        let mut responses = responses
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if responses.len() > 1 {
                            responses.pop_front().expect("scripted response")
                        } else {
                            responses.front().cloned().expect("scripted response")
                        }
                    }
                    // Decided from the request that was just recorded, so a
                    // rejected request neither consumes a turn nor perturbs
                    // the answer a later one gets.
                    Script::PerRequest(respond) => {
                        let request = captured_thread
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .last()
                            .cloned()
                            .expect("the request just recorded");
                        respond(&request)
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

            // `Drop` wakes this blocking accept with one connect: it is the
            // event that lets the loop observe the flag, so the connection is
            // discarded instead of served.
            while let Ok((stream, _)) = listener.accept() {
                if shutdown_thread.load(Ordering::SeqCst) {
                    break;
                }
                serve_request(stream);
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
    #[must_use]
    pub fn base_url(&self, prefix: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}",
            self.addr.port(),
            prefix.trim_matches('/')
        )
    }

    /// Every request captured so far, in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        // Signal the serve thread to stop, then connect once to wake it out of
        // `accept` — the connect IS the event the loop checks the flag on, so
        // teardown needs no poll interval and no sleep. A thread already inside
        // a request finishes it (its reads carry a short timeout) and then
        // observes the flag on the next accept.
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Bind the mock's loopback listener on an ephemeral port.
///
/// `127.0.0.1` (never a wildcard address) so the mock is unreachable from
/// outside the machine, and port 0 so parallel tests never collide.
///
/// # Panics
///
/// Panics if the bind fails; a test harness has no way to continue without a
/// listener.
fn bind_mock_listener() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
    let addr = listener.local_addr().expect("mock provider local addr");
    (listener, addr)
}
