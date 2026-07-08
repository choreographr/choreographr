use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tai_daemon::metrics;

/// Start a metrics server on a known port, scrape `/metrics`, and verify
/// that the response contains the expected OpenMetrics format.
#[test]
#[ignore]
fn metrics_server_serves_openmetrics_format() {
    let addr: std::net::SocketAddr = "127.0.0.1:19464".parse().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));

    let srv_shutdown = Arc::clone(&shutdown);
    let server_handle = thread::spawn(move || {
        metrics::serve_metrics(addr, srv_shutdown);
    });

    // Give the server a moment to start.
    thread::sleep(Duration::from_millis(200));

    // Send a raw HTTP GET request to /metrics.
    let response = fetch_metrics("127.0.0.1:19464");
    assert!(
        response.is_some(),
        "expected a response from metrics server"
    );
    let response = response.unwrap();

    // Should contain the expected OpenMetrics header and our metrics.
    assert!(
        response.contains("text/plain; version=0.0.4"),
        "unexpected content type in response: {response}"
    );
    assert!(
        response.contains("# HELP tai_sessions_active"),
        "missing HELP line: {response}"
    );
    assert!(
        response.contains("tai_connections_total 0"),
        "expected connections counter: {response}"
    );

    // Shut down the metrics server and wait for it to exit.
    shutdown.store(true, Ordering::SeqCst);
    // Connect to unblock recv_timeout immediately rather than waiting 1 s.
    let _ = TcpStream::connect("127.0.0.1:19464");
    let _ = server_handle.join();
}

/// Send a minimal HTTP GET request to the metrics server and return the
/// response body (header + body) as a string.
fn fetch_metrics(host: &str) -> Option<String> {
    let mut stream = TcpStream::connect(host).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let request = format!("GET /metrics HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    // Signal EOF on the write side so the server knows the request is complete.
    stream.shutdown(Shutdown::Write).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}
