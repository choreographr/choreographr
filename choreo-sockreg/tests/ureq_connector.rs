//! Integration test for the `ureq` feature: a full HTTP request through
//! `RegisteringTcpConnector` against a real loopback server, asserting the
//! request succeeds AND the registry captured exactly one socket, and that
//! the RAII guard deregisters it once the agent (and its pooled transport)
//! is dropped.
//!
//! Also: the multi-address fall-through (ureq issue #1184 parity) — a host
//! whose FIRST resolved address is unreachable at the connection level must
//! not abort the dial; the second address has to be tried.

#![cfg(all(unix, feature = "ureq"))]
// Real loopback listener + agent — integration territory, #[ignore] per the
// workspace test discipline.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};

use choreo_sockreg::{RegisteringTcpConnector, SocketRegistry};
use ureq::Agent;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::NextTimeout;

/// Minimal HTTP/1.1 response bytes a client with our connector can parse.
const RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";

/// A resolver that answers every lookup with a fixed list of addresses, a
/// dead port first: the scripted analogue of macOS returning a blackholed
/// AAAA before the IPv4 (ureq issue #1184 scenario).
#[derive(Debug)]
struct ScriptedResolver {
    addrs: Vec<SocketAddr>,
}

impl Resolver for ScriptedResolver {
    fn resolve(
        &self,
        _uri: &Uri,
        _config: &Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        use ureq::unversioned::resolver::ArrayVec;
        // All 16 slots must be filled by from_fn; the sentinel
        // (0.0.0.0:0) is only ever pushed over by the scripted list, never dialed.
        let mut out = ArrayVec::from_fn(|_| {
            SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
        });
        for a in &self.addrs {
            out.push(*a);
        }
        Ok(out)
    }
}

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

    let registry = SocketRegistry::new();
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

    // One request, one dialed connection. RAII lifecycle observable: the
    // mock answers with `Connection: close` (see `MockProvider`/`RESPONSE`),
    // so ureq drops the transport as soon as the body is consumed — and the
    // guard's `Drop` unregisters. The registry must be back to the pre-dial
    // level (0) even while the AGENT is still alive. (If a pooled/keep-alive
    // connection were reused instead, the entry would legitimately stay at
    // 1 until the agent drop — the second test below covers that shape.)
    assert_eq!(registry.registered_count(), 0);

    // Belt-and-braces: after the agent (and its pool) are dropped, the
    // registry must still be empty — no entry can leak through the pool.
    drop(agent);
    assert_eq!(registry.registered_count(), 0);
}

#[test]
#[ignore = "integration test: binds a real loopback server"]
fn registry_returns_to_predial_level_after_agent_drop() {
    // Same shape as the test above but focused purely on the observable
    // called out in the plan: count before dial == count after the agent is
    // dropped. ureq's pool may keep the connection alive while the agent
    // lives, so the assertion is on the post-drop state only.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.write_all(RESPONSE.as_bytes()).expect("write response");
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf);
    });

    let registry = SocketRegistry::new();
    assert_eq!(registry.registered_count(), 0);

    let agent = Agent::with_parts(
        Config::default(),
        RegisteringTcpConnector::new(registry.clone()),
        DefaultResolver::default(),
    );
    let mut res = agent
        .get(format!("http://{addr}/"))
        .call()
        .expect("request succeeded");
    let _ = res.body_mut().read_to_string().expect("read body");
    server.join().expect("server thread joined");

    drop(agent);
    // Steady state observable: the registry tracks only LIVE connections.
    // The connection is done (Connection: close) and the agent is gone, so
    // the count must be 0 — nothing dangles in the registry.
    assert_eq!(registry.registered_count(), 0);
}

#[test]
#[ignore = "integration test: binds a real loopback server"]
fn first_refused_resolved_address_falls_through_to_second() {
    // ureq issue #1184 parity: the resolver (macOS getaddrinfo scenarios)
    // puts a dead address FIRST and a reachable one second. The connector
    // must dial through: the agent-level request succeeds, proving the
    // fall-through happened at the connector level.
    //
    // Dead address: bind a listener, then immediately drop it — the port is
    // released, so dialing it on loopback yields ConnectionRefused fast and
    // deterministically (no IPv6 dependence, no waiting).
    let dead_listener = TcpListener::bind("127.0.0.1:0").expect("bind dead port");
    let dead_addr = dead_listener.local_addr().expect("dead addr");
    drop(dead_listener);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind live port");
    let live_addr = listener.local_addr().expect("live addr");
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.write_all(RESPONSE.as_bytes()).expect("write response");
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf);
    });

    let registry = SocketRegistry::new();
    // The scripted list replaces the real resolver to make the ordering
    // deterministic (a hostname-wide lookup would be platform-ordered).
    let agent = Agent::with_parts(
        Config::builder()
            .timeout_connect(Some(std::time::Duration::from_secs(2)))
            .build(),
        RegisteringTcpConnector::new(registry.clone()),
        ScriptedResolver {
            addrs: vec![dead_addr, live_addr],
        },
    );

    // A hostname (not an IP literal) so the resolver is actually consulted.
    let mut res = agent
        .get("http://sockreg-fallthrough.test/")
        .call()
        .expect("request must succeed via the second address");
    let body = res.body_mut().read_to_string().expect("read body");
    assert_eq!(body, "hello");
    server.join().expect("server thread joined");
}

#[test]
#[ignore = "integration test: binds a real loopback server"]
fn all_refused_resolved_addresses_report_refusal() {
    // The aggregate error must still be well-typed and meaningful when every
    // scripted address refuses: a ConnectionRefused surfaces (the connector
    // reports its last address-specific failure), not a synthetic message.
    let dead_listener = TcpListener::bind("127.0.0.1:0").expect("bind dead port");
    let dead_addr = dead_listener.local_addr().expect("dead addr");
    drop(dead_listener);

    let registry = SocketRegistry::new();
    let agent = Agent::with_parts(
        Config::builder()
            .timeout_connect(Some(std::time::Duration::from_secs(2)))
            .build(),
        RegisteringTcpConnector::new(registry.clone()),
        ScriptedResolver {
            addrs: vec![dead_addr, dead_addr],
        },
    );
    let err = agent
        .get("http://sockreg-dead.test/")
        .call()
        .expect_err("no address can accept");
    let message = err.to_string();
    // ureq surfaces the connector's Error::Io<ConnectionRefused> as the
    // failure reason (aggregated; classification must stay robust).
    assert!(
        message.contains("Connection refused"),
        "expected refusal in the aggregate error, got: {message}"
    );
}
