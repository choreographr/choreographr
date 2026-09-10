//! Integration test for the `ureq` feature: a full HTTP request through
//! `RegisteringTcpConnector` against a real loopback server, asserting the
//! request succeeds AND the registry captured exactly one socket.

#![cfg(all(unix, feature = "ureq"))]
// Real loopback listener + agent — integration territory, #[ignore] per the
// workspace test discipline.

use std::io::{Read, Write};
use std::net::TcpListener;

use choreo_sockreg::RegisteringTcpConnector;
use ureq::Agent;
use ureq::config::Config;
use ureq::unversioned::resolver::DefaultResolver;
/// Minimal HTTP/1.1 response bytes a client with our connector can parse.
const RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";

#[test]
#[ignore = "integration test: binds a real loopback server"]
fn ureq_request_through_registering_connector() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    // One-shot server thread: accept once, answer once, close. std mpsc is
    // fine here per the house rules (leaf crate, trivial single-consumer).
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.write_all(RESPONSE.as_bytes()).expect("write response");
        // Read the request off the wire (ureq wrote it before waiting for
        // the response); we don't parse it, just drain a bounded amount so
        // the client isn't left with a full send buffer.
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf);
    });

    let registry = choreo_sockreg::SocketRegistry::new();
    // Plain-TCP connector chain with nothing chained in front: exactly the
    // shape Agent::with_parts is meant for (mirrors ureq's own doc example,
    // minus TLS).
    let agent = Agent::with_parts(
        Config::default(),
        RegisteringTcpConnector::new(registry.clone()),
        DefaultResolver::default(),
    );

    let mut res = agent
        .get(format!("http://{addr}/"))
        .call()
        .expect("request succeeded");
    let body = res.body_mut().read_to_string().expect("read body");
    assert_eq!(body, "hello");
    server.join().expect("server thread joined");

    // One request, one connection, one registered socket.
    assert_eq!(registry.registered_count(), 1);
}
